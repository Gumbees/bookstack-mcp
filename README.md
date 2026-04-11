# BookStack MCP Server

An MCP (Model Context Protocol) server that gives Claude full access to a [BookStack](https://www.bookstackapp.com/) instance. Built in Rust with tokio/axum as a Cargo workspace with pluggable database backends and optional semantic vector search.

## Features

- Full CRUD on all core BookStack resources (shelves, books, chapters, pages, attachments)
- Full-text search with BookStack query operators
- **Semantic vector search** — natural language search across all content via embeddings (optional)
- **Pluggable database** — SQLite for simple deployments, PostgreSQL + pgvector for production
- **Separate embedder** — background embedding service with pluggable backends (local ONNX, Ollama, OpenAI)
- **Server-side markdown to HTML conversion** — send markdown, server converts before sending to BookStack
- **Staging upload flow** — upload local images and attachments through a two-step staging endpoint without exposing local paths to the container ([see below](#uploading-local-files-images--attachments))
- **OAuth 2.1 support** — use as a Claude.ai or Claude Desktop custom connector without config files
- **Encrypted token storage** — OAuth tokens encrypted at rest with AES-256-GCM
- **Dual transport** — SSE (MCP 2024-11-05) and Streamable HTTP (MCP 2025-03-26)
- **AI instance summary** — optional LLM-generated summary of your knowledge base included in MCP context
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
  bsmcp-embedder/     Embedder binary (local ONNX / Ollama / OpenAI, job queue worker + HTTP /embed)

docker/
  Dockerfile.server       Lightweight server image (~35MB)
  Dockerfile.embedder     Embedder image with ONNX Runtime (~45MB)
  docker-compose.yml      PostgreSQL deployment (production)
  docker-compose.sqlite.yml  SQLite deployment (simple)
```

The MCP server handles all client-facing protocol, OAuth, and search. The embedder runs separately, polling a database-backed job queue to embed pages and serving a `/embed` HTTP endpoint for query-time embedding. The embedder supports three backends: local ONNX models (fastembed), Ollama, or OpenAI-compatible APIs.

## Available Tools (61)

| Category | Tools |
|----------|-------|
| **Search** | `search_content` |
| **Semantic** | `semantic_search`, `reembed`, `embedding_status` |
| **Shelves** | `list_shelves`, `get_shelf`, `create_shelf`, `update_shelf`, `delete_shelf` |
| **Books** | `list_books`, `get_book`, `create_book`, `update_book`, `delete_book` |
| **Chapters** | `list_chapters`, `get_chapter`, `create_chapter`, `update_chapter`, `delete_chapter` |
| **Pages** | `list_pages`, `get_page`, `create_page`, `update_page`, `delete_page`, `edit_page`, `append_to_page`, `replace_section`, `insert_after` |
| **Move** | `move_page`, `move_chapter`, `move_book_to_shelf` |
| **Attachments** | `list_attachments`, `get_attachment`, `create_attachment`, `update_attachment`, `delete_attachment`, `upload_attachment` |
| **Exports** | `export_page`, `export_chapter`, `export_book` (markdown, plaintext, html) |
| **Comments** | `list_comments`, `get_comment`, `create_comment`, `update_comment`, `delete_comment` |
| **Recycle Bin** | `list_recycle_bin`, `restore_recycle_bin_item`, `destroy_recycle_bin_item` |
| **Users** | `list_users`, `get_user` |
| **Audit Log** | `list_audit_log` |
| **System** | `get_system_info` |
| **Images** | `list_images`, `get_image`, `upload_image`, `update_image`, `delete_image` |
| **Permissions** | `get_content_permissions`, `update_content_permissions` |
| **Roles** | `list_roles`, `get_role` |

Semantic tools (`semantic_search`, `reembed`, `embedding_status`) only appear when `BSMCP_SEMANTIC_SEARCH=true` and an embedder is running. Without semantic search: 58 tools.

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
| `BSMCP_ACCESS_TOKEN_TTL` | No | `86400` | Access token TTL in seconds (24h) |
| `BSMCP_REFRESH_TOKEN_TTL` | No | `7776000` | Refresh token TTL in seconds (90d) |
| `BSMCP_BACKUP_INTERVAL` | No | - | Hours between backups (0 = disabled) |
| `BSMCP_BACKUP_PATH` | No | `/data/backups` | Backup directory |
| `BSMCP_LLM_PROVIDER` | No | - | LLM for instance summary: `openrouter`, `anthropic`, `openai`, `ollama` |
| `BSMCP_LLM_API_KEY` | No | - | API key for LLM provider (not needed for ollama) |
| `BSMCP_LLM_MODEL` | No | (per provider) | Model ID for summary generation |
| `BSMCP_LLM_API_URL` | No | (per provider) | Base URL for LLM API (useful for ollama on different host) |
| `BSMCP_SUMMARY_INTERVAL` | No | `0` | Hours between summary regeneration (0 = only on first startup) |
| `BSMCP_SUMMARY_TOKEN_ID` | No | - | BookStack token for summary (falls back to `BSMCP_EMBED_TOKEN_*`) |
| `BSMCP_SUMMARY_TOKEN_SECRET` | No | - | BookStack token secret for summary |

#### Embedder Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `BSMCP_EMBED_TOKEN_ID` | Yes | - | BookStack API token ID for crawling |
| `BSMCP_EMBED_TOKEN_SECRET` | Yes | - | BookStack API token secret |
| `BSMCP_EMBED_PROVIDER` | No | `local` | Embedding backend: `local`, `ollama`, `openai` |
| `BSMCP_EMBED_MODEL` | No | (per provider) | Model name (see [Embedding Providers](#embedding-providers)) |
| `BSMCP_EMBED_API_KEY` | If openai | - | API key for OpenAI embedding provider |
| `BSMCP_EMBED_API_URL` | No | (per provider) | Base URL for Ollama or OpenAI-compatible endpoint |
| `BSMCP_EMBED_DIMS` | No | (auto) | Embedding dimensions (auto-detected for Ollama) |
| `BSMCP_MODEL_PATH` | No | `/data/models` | ONNX model cache directory (local provider only) |
| `BSMCP_EMBED_CPUS` | No | `0` (unlimited) | Docker CPU limit for embedder |
| `BSMCP_EMBED_JOB_TIMEOUT` | No | `14400` | Seconds before stuck jobs reset |
| `BSMCP_EMBED_BATCH_SIZE` | No | `32` | Chunks per embedding batch |
| `BSMCP_EMBED_DELAY_MS` | No | `50` | Delay between pages (API throttle) |
| `BSMCP_EMBED_POLL_INTERVAL` | No | `5` | Seconds between job queue polls |
| `BSMCP_EMBED_ON_STARTUP` | No | `false` | `true` = auto-embed on startup, `clean` = clear all embeddings first |
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
| `GET` | `/status` | Embedding progress page with live progress bar |
| `GET` | `/.well-known/oauth-authorization-server` | OAuth metadata (RFC 8414) |
| `GET` | `/.well-known/oauth-protected-resource` | Protected resource metadata (RFC 9728) |
| `GET` | `/authorize` | Login form for BookStack API token |
| `POST` | `/authorize` | Validate credentials and issue auth code |
| `POST` | `/token` | OAuth token exchange |
| `POST` | `/register` | Dynamic client registration (RFC 7591) |

## Upgrading

All schema migrations are automatic on startup (CREATE TABLE IF NOT EXISTS, ALTER TABLE for new columns). No manual SQL is needed.

### From v0.5.2 to v0.5.3

v0.5.3 fixes embedding dimension detection, adds Ollama LLM support for summaries, and improves hybrid search scoring.

#### What's new

- **Ollama LLM support** — `BSMCP_LLM_PROVIDER=ollama` for instance summaries using local models (no API key needed)
- **Configurable summary refresh** — `BSMCP_SUMMARY_INTERVAL` (hours) for periodic regeneration instead of one-time only
- **Configurable LLM base URL** — `BSMCP_LLM_API_URL` for remote Ollama instances or custom endpoints
- **Hybrid search scoring fix** — keyword-only results no longer inflate above actual semantic matches via blanket boost. Pages with zero vector similarity are capped below real semantic results.
- **Embedding dimension auto-detection fix** — empty `BSMCP_EMBED_DIMS` env var no longer bypasses Ollama dimension detection (was silently defaulting to 768)
- **Auto-reindex on dimension change** — embedder now detects stored vs actual dimensions and triggers clean reindex automatically

#### What you must do

1. **Pull new images**: `ghcr.io/bees-roadhouse/bsmcp-server:0.5.3` + `ghcr.io/bees-roadhouse/bsmcp-embedder:0.5.3`
2. **Restart** — dimension mismatch auto-reindexes if needed

### From v0.5.1 to v0.5.2

v0.5.2 adds pluggable embedding providers, AI instance summaries, OAuth refresh tokens, and several quality-of-life improvements.

#### What's new

- **Embedding providers** — choose between local ONNX (`local`), Ollama (`ollama`), or OpenAI (`openai`) via `BSMCP_EMBED_PROVIDER`. Ollama auto-detects dimensions. OpenAI works with any compatible endpoint.
- **AI instance summary** — optional LLM call at startup generates a contextual summary of the knowledge base, included in MCP instructions so connecting AI assistants immediately understand what this BookStack is about. Supports OpenRouter, Anthropic, and OpenAI.
- **OAuth refresh tokens** — clients no longer need to re-enter API credentials every 24 hours. Refresh tokens silently issue new access tokens as long as BookStack credentials remain valid.
- **Configurable token TTLs** — `BSMCP_ACCESS_TOKEN_TTL` and `BSMCP_REFRESH_TOKEN_TTL` env vars.
- **Job queue status page** — `/status` now shows all pending/running jobs with progress bars plus recent completed/failed jobs.
- **Similar-page computation** — runs after every embedding job, not just full reindexes.
- **WYSIWYG editing** — all editing tools (`edit_page`, `replace_section`, `append_to_page`, `insert_after`) now explicitly documented to work on WYSIWYG pages.
- **Duplicate title prevention** — instructions tell AI not to include page title as H1 in content.
- **Auto-migration fix** — handles pre-semantic SQLite databases that lack `pages` table.

#### What's automatic

- All schema changes (refresh_tokens table, etc.) are applied on startup
- Existing deployments continue working with no env var changes
- Local ONNX embedding remains the default if `BSMCP_EMBED_PROVIDER` is not set

#### What you must do

1. **Pull new images**: `ghcr.io/bees-roadhouse/bsmcp-server:0.5.2` + `ghcr.io/bees-roadhouse/bsmcp-embedder:0.5.2` (or use `latest`)
2. **Restart** — that's it for the base upgrade

**Optional: Enable AI instance summary** — add LLM env vars:
```bash
BSMCP_LLM_PROVIDER=openrouter  # or: anthropic, openai, ollama
BSMCP_LLM_API_KEY=your-api-key  # not needed for ollama
BSMCP_SUMMARY_INTERVAL=24       # regenerate every 24h (0 = only on first startup)
# Uses BSMCP_EMBED_TOKEN_ID/SECRET for BookStack API access
```

**Optional: Switch to Ollama/OpenAI embeddings** — set `BSMCP_EMBED_PROVIDER`:
```bash
BSMCP_EMBED_PROVIDER=ollama
BSMCP_EMBED_MODEL=nomic-embed-text
BSMCP_EMBED_API_URL=http://ollama:11434
```
Switching provider triggers an automatic clean re-index.

### From v0.5.0 to v0.5.1

v0.5.1 switches the default embedding model and adds automatic model change detection.

#### What's new

- **Default model: EmbeddingGemma-300M** — Google's lightweight embedding model (768 dims, 300M params). Faster and lighter than BGE-large, especially on ARM.
- **Model change detection** — embedder detects model changes via meta table and auto-triggers clean re-index with pgvector dimension adjustment
- **Configurable embedding dimensions** — pgvector column type automatically adjusts when switching models
- **HuggingFace model downloads** — custom ONNX models download automatically from HuggingFace Hub

#### What's automatic

- **Full re-index** — switching from BGE-large (1024 dims) to EmbeddingGemma (768 dims) triggers automatic clean re-index. PostgreSQL column type is altered automatically.
- No env var changes required unless you want to keep the old model

#### What you must do

1. **Pull new images**: `ghcr.io/bees-roadhouse/bsmcp-server:0.5.1` + `ghcr.io/bees-roadhouse/bsmcp-embedder:0.5.1`
2. **Restart** — the embedder auto-detects the model change and re-indexes. Check progress at `/status`.
3. **To keep the old model**: Set `BSMCP_EMBED_MODEL=BAAI/bge-large-en-v1.5` in your embedder env.

### From v0.4.0 to v0.5.0

v0.5.0 is a search quality release — no infrastructure changes, just better results.

#### What's new

- **Hybrid search** — combines vector similarity with BookStack keyword search, weighted blend (70% vector + 20% keyword + blanket boost)
- **Markov blanket re-ranking** — pages whose graph neighbors also scored get a relevance boost (up to +0.15)
- **Tighter chunking** — max chunk size reduced from 2000 to 1200 chars with 150-char paragraph overlap between chunks
- **Higher default threshold** — raised from 0.50 to 0.65 to filter out low-quality matches
- **Auto-reindex on upgrade** — chunk version tracking triggers automatic clean re-index when chunking logic changes
- **`meta` table** — new key-value metadata table in both SQLite and PostgreSQL backends

#### What's automatic

- **Full re-index** — the embedder detects the chunk version change (v1 → v2) and automatically clears all embeddings and re-indexes everything on first startup. No manual `reembed` needed.
- Schema migration — `meta` table created automatically on startup
- All existing env vars and compose files are compatible

#### What you must do

1. **Pull new images**: `ghcr.io/bees-roadhouse/bsmcp-server:0.5.0` + `ghcr.io/bees-roadhouse/bsmcp-embedder:0.5.0`
2. **Restart** — the embedder auto-detects the chunk version change and re-indexes. Check progress at `/status`.
3. **No env var changes required** — new `hybrid` parameter defaults to `true` in the `semantic_search` tool

#### New `semantic_search` parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `threshold` | `0.65` | Minimum score (was 0.50 in v0.4.0) |
| `hybrid` | `true` | Enable keyword + vector blended search |

Results now include a `scoring` breakdown when hybrid mode is on, showing vector, keyword, and blanket_boost components.

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
   - Old: single `ghcr.io/bees-roadhouse/bookstack-mcp:latest` container
   - New: `ghcr.io/bees-roadhouse/bsmcp-server:latest` + `ghcr.io/bees-roadhouse/bsmcp-embedder:latest`
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
   - Old: `docker-compose.yml` with `ghcr.io/bees-roadhouse/bookstack-mcp:latest`
   - New (SQLite): `docker/docker-compose.sqlite.yml`
   - New (PostgreSQL): `docker/docker-compose.yml`
   - Images: `ghcr.io/bees-roadhouse/bsmcp-server:latest` + `ghcr.io/bees-roadhouse/bsmcp-embedder:latest`

4. **Create a BookStack API token** for the embedder with read access to all content

5. **Configure webhook** in BookStack (see [Semantic Search Setup](#semantic-search-setup))

6. **Trigger initial embedding** via the `reembed` MCP tool

### From v0.1.2 to v0.1.3

See the [v0.1.3 release notes](https://github.com/bees-roadhouse/bookstack-mcp/releases/tag/v0.1.3):
- New required `BSMCP_ENCRYPTION_KEY` env var
- `BSMCP_PUBLIC_URL` renamed to `BSMCP_PUBLIC_DOMAIN`
- Docker volume renamed `mcp-data` to `bsmcp-data`
- PKCE enforcement for OAuth

## Embedding Providers

Set via `BSMCP_EMBED_PROVIDER`. Changing provider or model triggers an automatic clean re-index.

### Local (default)

Uses fastembed with ONNX Runtime. No external API needed but requires the heavier embedder container.

| Model Name | Dimensions | Parameters | Notes |
|------------|-----------|------------|-------|
| `BAAI/bge-base-en-v1.5` | 768 | 110M | **Default.** Good balance of speed and quality. |
| `BAAI/bge-large-en-v1.5` | 1024 | 335M | Highest quality, heavier. |
| `BAAI/bge-small-en-v1.5` | 384 | 33M | Fastest, lower quality. |
| `embeddinggemma-300m` | 768 | 300M | Google's lightweight model. |

### Ollama

Uses a local or remote Ollama instance. Dimensions auto-detected. No API key needed.

```bash
BSMCP_EMBED_PROVIDER=ollama
BSMCP_EMBED_MODEL=nomic-embed-text        # or any Ollama embedding model
BSMCP_EMBED_API_URL=http://ollama:11434    # default: http://localhost:11434
```

### OpenAI

Uses OpenAI's embedding API or any OpenAI-compatible endpoint.

```bash
BSMCP_EMBED_PROVIDER=openai
BSMCP_EMBED_MODEL=text-embedding-3-small   # default
BSMCP_EMBED_API_KEY=sk-...
BSMCP_EMBED_DIMS=1536                      # must match model output
BSMCP_EMBED_API_URL=https://api.openai.com # or any compatible endpoint
```

## Search Operators

The `search_content` tool supports BookStack's search operators:

- `"exact phrase"` - Exact match
- `{type:page}` - Filter by type (page, chapter, book, shelf)
- `{in_name:term}` - Search within names only
- `{created_by:me}` - Filter by creator
- `[tag_name=value]` - Filter by tag

## Uploading Local Files (Images & Attachments)

The MCP server runs in a container and cannot read files from the client machine's filesystem directly. To upload local images or file attachments, use the two-step **staging upload flow**:

**Step 1:** Call `prepare_upload` — returns a `staging_id` and a full `upload_url`:

```json
{
  "staging_id": "f0103f6c-7c98-46c2-adbe-606ba26937c3",
  "upload_url": "https://your-mcp-host/stage/upload/f0103f6c-7c98-46c2-adbe-606ba26937c3",
  "ttl_seconds": 300
}
```

**Step 2:** POST the file to `upload_url` as multipart form-data. No auth header needed — the `staging_id` (a UUID that can only be generated via an authenticated MCP call) acts as the auth token for the one-time upload:

```bash
curl -X POST -F "file=@/path/to/image.jpg" \
  "https://your-mcp-host/stage/upload/f0103f6c-7c98-46c2-adbe-606ba26937c3"
```

**Step 3:** Call `upload_image` (or `upload_attachment`) with the `staging_id`:

```json
{
  "name": "Banner Logo",
  "uploaded_to": 1908,
  "staging_id": "f0103f6c-7c98-46c2-adbe-606ba26937c3",
  "mime_type": "image/jpeg",
  "embed": true
}
```

The staging slot is **consumed on first use** (destructively removed from the store) and **auto-expires after 5 minutes**. Maximum file size is 50MB.

### The `embed` parameter

`upload_image` accepts an `embed` boolean parameter (default `false`). When `embed=true`, the image is automatically appended to the target page's content after uploading, so you don't need a separate `edit_page` or `append_to_page` call. Works for both markdown and WYSIWYG pages.

### Alternative: `url` parameter

If the file is already hosted at a public URL the MCP server can reach, you can skip the staging flow entirely and pass the `url` parameter directly to `upload_image` or `upload_attachment`. The server will fetch the file and forward it to BookStack.

### Currently Claude Code only

**The staging upload flow currently only works from [Claude Code](https://claude.com/claude-code) (the CLI tool).** It does not work from Claude.ai's web custom connectors or Claude Desktop custom connectors.

The reason: Step 2 requires the MCP client to make an outbound HTTP POST to the MCP server's staging endpoint with the file bytes. Claude Code runs locally and has shell access (via its `Bash` tool), so it can `curl` the file directly. Claude.ai's remote MCP connector runs the MCP client inside Anthropic's sandboxed proxy infrastructure, which does not expose a mechanism for the client to make arbitrary HTTP file uploads to third-party hosts. Claude Desktop has similar limitations today.

If you're using Claude.ai or Claude Desktop, you can still use `upload_image` with the `url` parameter for files that are already web-accessible, or upload through the BookStack web UI directly.

## License

MIT
