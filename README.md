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

## Available Tools (56)

| Category | Tools |
|----------|-------|
| **Search** | `search_content` |
| **Semantic** | `semantic_search`, `reembed`, `embedding_status` |
| **Shelves** | `list_shelves`, `get_shelf`, `create_shelf`, `update_shelf`, `delete_shelf` |
| **Books** | `list_books`, `get_book`, `create_book`, `update_book`, `delete_book` |
| **Chapters** | `list_chapters`, `get_chapter`, `create_chapter`, `update_chapter`, `delete_chapter` |
| **Pages** | `list_pages`, `get_page`, `create_page`, `update_page`, `delete_page`, `edit_page`, `append_to_page`, `replace_section`, `insert_after` |
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

Semantic tools (`semantic_search`, `reembed`, `embedding_status`) only appear when `BSMCP_SEMANTIC_SEARCH=true` and an embedder is running. Without semantic search: 53 tools.

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
| `BSMCP_EMBED_CPUS` | No | `0` (unlimited) | Docker CPU limit for embedder |
| `BSMCP_EMBED_JOB_TIMEOUT` | No | `14400` | Seconds before stuck jobs reset |
| `BSMCP_EMBED_BATCH_SIZE` | No | `32` | Chunks per embedding batch |
| `BSMCP_EMBED_DELAY_MS` | No | `50` | Delay between pages (API throttle) |
| `BSMCP_EMBED_POLL_INTERVAL` | No | `5` | Seconds between job queue polls |
| `BSMCP_EMBED_ON_STARTUP` | No | `false` | Auto-queue a full embed job on startup |
| `BSMCP_EMBED_HOST` | No | `0.0.0.0` | Embedder listen address |
| `BSMCP_EMBED_PORT` | No | `8081` | Embedder listen port |

See `.env.example` for the full list with comments.

### Semantic Search Setup

1. Set `BSMCP_SEMANTIC_SEARCH=true` in your server env
2. Set `BSMCP_WEBHOOK_SECRET` to a random string (16+ characters)
3. Create a BookStack API token with read access for the embedder (`BSMCP_EMBED_TOKEN_ID` / `BSMCP_EMBED_TOKEN_SECRET`)
4. Start the embedder container — it downloads the ONNX model (~1.3GB) on first run
5. Use the `reembed` tool (via Claude) to trigger initial embedding of all pages
6. Configure a webhook in BookStack for automatic re-embedding on page changes:

#### BookStack Webhook Configuration

Go to **Settings > Webhooks > Create Webhook** in your BookStack instance:

| Field | Value |
|-------|-------|
| **Name** | MCP Semantic Search |
| **Endpoint URL** | `https://your-mcp-host/webhooks/bookstack` |
| **Active** | Yes |

**Events to select:**
- Page Create
- Page Update
- Page Delete

**Custom header** (required for verification):
```
X-Webhook-Secret: YOUR_WEBHOOK_SECRET
```

The `YOUR_WEBHOOK_SECRET` value must match `BSMCP_WEBHOOK_SECRET` in your server environment. The server uses constant-time comparison to verify the header.

After saving, any page create/update/delete in BookStack automatically queues a re-embedding job. The embedder picks it up within seconds (configurable via `BSMCP_EMBED_POLL_INTERVAL`).

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
| `POST` | `/webhooks/bookstack` | BookStack webhook receiver (semantic search) |
| `GET` | `/.well-known/oauth-authorization-server` | OAuth metadata (RFC 8414) |
| `GET` | `/.well-known/oauth-protected-resource` | Protected resource metadata (RFC 9728) |
| `GET` | `/authorize` | Login form for BookStack API token |
| `POST` | `/authorize` | Validate credentials and issue auth code |
| `POST` | `/token` | OAuth token exchange |
| `POST` | `/register` | Dynamic client registration (RFC 7591) |

## Upgrading

All schema migrations are automatic on startup (CREATE TABLE IF NOT EXISTS, ALTER TABLE for new columns). No manual SQL is needed.

### From v0.3.x to v0.4.0

v0.4.0 splits the monolithic `bookstack-mcp` container into separate **server** and **embedder** binaries with a pluggable database layer (SQLite or PostgreSQL + pgvector).

#### What's new

- **Separate containers** — `bsmcp-server` (MCP protocol, OAuth, search) and `bsmcp-embedder` (ONNX model, background embedding, `/embed` HTTP endpoint)
- **PostgreSQL + pgvector** — optional production backend with native HNSW vector indexing
- **Database-backed job queue** — embedding jobs persist across restarts
- **Auto-migration** — switch `BSMCP_DB_BACKEND=postgres` and the server migrates SQLite data automatically
- **Dual MCP transport** — SSE (2024-11-05) and Streamable HTTP (2025-03-26)
- **New page editing tools** — `edit_page`, `append_to_page`, `replace_section`, `insert_after`

#### What's automatic

- SQLite schema is compatible — same tables, same columns
- `worker_id` column auto-added to `embed_jobs` if missing
- Existing embeddings preserved (same model: `BAAI/bge-large-en-v1.5`, same 1024 dimensions)
- Auto-migration from SQLite to PostgreSQL when switching backends

#### What you must do

1. **Replace compose file and images**:
   - Old: single `ghcr.io/gumbees/bookstack-mcp:latest` container
   - New: `ghcr.io/gumbees/bsmcp-server:0.4.0` + `ghcr.io/gumbees/bsmcp-embedder:0.4.0`
   - Use `docker/docker-compose.sqlite.yml` (simple) or `docker/docker-compose.yml` (PostgreSQL)

2. **Add new env vars**:
   ```bash
   # Database backend (required)
   BSMCP_DB_BACKEND=sqlite   # or postgres

   # Embedder connection (required for semantic search)
   BSMCP_EMBEDDER_URL=http://bsmcp-embedder:8081

   # Separate BookStack API token for the embedder (required for semantic search)
   BSMCP_EMBED_TOKEN_ID=<BookStack API token ID>
   BSMCP_EMBED_TOKEN_SECRET=<BookStack API token secret>

   # PostgreSQL (only if switching to postgres)
   BSMCP_DATABASE_URL=postgres://bsmcp:yourpassword@bsmcp-postgres/bsmcp
   BSMCP_DB_PASSWORD=yourpassword
   ```

3. **`BSMCP_EMBED_THREADS` is removed** — use `BSMCP_EMBED_CPUS` (Docker CPU limit) instead.

4. **Update webhook** to use `X-Webhook-Secret` header instead of `?secret=` query param (query param still works but is deprecated).

#### Migrating to PostgreSQL

Set `BSMCP_DB_BACKEND=postgres` and keep the SQLite file accessible at `BSMCP_DB_PATH`. The server auto-migrates all data on startup and renames the SQLite file to `.db.migrated`.

Manual migration is also available:
```bash
docker exec bsmcp-server bsmcp-server migrate \
  --from-sqlite /data/bookstack-mcp.db \
  --to-postgres postgres://bsmcp:yourpassword@bsmcp-postgres/bsmcp
```

Migration copies encrypted tokens as-is (portable when `BSMCP_ENCRYPTION_KEY` matches), converts embeddings from BLOB to pgvector format, and fixes PostgreSQL sequences.

### From v0.1.x to v0.4.0

This is the largest jump — from a single monolithic container with no encryption and no semantic search to the full multi-container architecture.

#### What's automatic

- Plaintext tokens from v0.1.0-0.1.2 are transparently encrypted on first access (the server detects unencrypted values and re-encrypts them in place)
- All database tables are created on startup via `CREATE TABLE IF NOT EXISTS`

#### What you must do

1. **Docker volume rename** (v0.1.0-0.1.2 only — skip if already on v0.1.3+):
   ```bash
   docker compose down
   docker volume create bsmcp-data
   docker run --rm -v mcp-data:/source:ro -v bsmcp-data:/dest alpine cp -a /source/. /dest/
   docker volume rm mcp-data  # after verification
   ```

2. **Update env vars**:
   ```bash
   # REMOVE (no longer recognized):
   # BSMCP_PUBLIC_URL=https://mcp.example.com

   # ADD (required):
   BSMCP_ENCRYPTION_KEY=<generate: openssl rand -base64 48>
   BSMCP_PUBLIC_DOMAIN=mcp.example.com  # domain only, no https://

   # ADD (for semantic search):
   BSMCP_SEMANTIC_SEARCH=true
   BSMCP_WEBHOOK_SECRET=<random string, 16+ chars>
   BSMCP_EMBED_TOKEN_ID=<BookStack API token ID>
   BSMCP_EMBED_TOKEN_SECRET=<BookStack API token secret>
   BSMCP_EMBEDDER_URL=http://bsmcp-embedder:8081

   # ADD (for PostgreSQL — recommended):
   BSMCP_DB_BACKEND=postgres
   BSMCP_DATABASE_URL=postgres://bsmcp:yourpassword@bsmcp-postgres/bsmcp
   BSMCP_DB_PASSWORD=yourpassword
   ```

3. **Replace compose file entirely**:
   - Old: `docker-compose.yml` with `ghcr.io/gumbees/bookstack-mcp:latest`
   - New (SQLite): `docker/docker-compose.sqlite.yml`
   - New (PostgreSQL): `docker/docker-compose.yml`
   - Images: `ghcr.io/gumbees/bsmcp-server:0.4.0` + `ghcr.io/gumbees/bsmcp-embedder:0.4.0`

4. **Create a BookStack API token** for the embedder with read access to all content

5. **Configure webhook** in BookStack (see [Semantic Search Setup](#semantic-search-setup))

6. **Trigger initial embedding** via the `reembed` MCP tool

### From v0.1.2 to v0.1.3

See the [v0.1.3 release notes](https://github.com/gumbees/bookstack-mcp/releases/tag/v0.1.3):
- New required `BSMCP_ENCRYPTION_KEY` env var
- `BSMCP_PUBLIC_URL` renamed to `BSMCP_PUBLIC_DOMAIN`
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
