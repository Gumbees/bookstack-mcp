# BookStack MCP Server

Rust MCP server that bridges Claude to a BookStack instance via SSE and Streamable HTTP transports. Organized as a Cargo workspace with pluggable database backends and a separate embedder service.

## Architecture

```
crates/
  bsmcp-common/          Shared types, traits, config
    src/lib.rs           Re-exports
    src/config.rs        Env var parsing, DbBackendType
    src/db.rs            DbBackend + SemanticDb traits (async)
    src/types.rs         PageMeta, ChunkData, EmbedJob, MarkovBlanket, SearchHit, etc.
    src/chunking.rs      Markdown chunking (heading-aware, ~500 token chunks)
    src/vector.rs        BLOB↔embedding conversion, cosine similarity

  bsmcp-db-sqlite/       SQLite backend
    src/lib.rs           impl DbBackend + SemanticDb for SqliteDb
                         rusqlite wrapped in spawn_blocking for async
                         Brute-force cosine scan for vector search

  bsmcp-db-postgres/     PostgreSQL + pgvector backend
    src/lib.rs           impl DbBackend + SemanticDb for PostgresDb
                         sqlx async driver
                         HNSW index for vector search (embedding <=> operator)
                         FOR UPDATE SKIP LOCKED for concurrent job queue

  bsmcp-server/          MCP server binary
    src/main.rs          Axum server, routes, env config, CORS, db backend selection, auto-migration
    src/sse.rs           SSE session management, Streamable HTTP, multi-user auth
    src/mcp.rs           MCP protocol handler, tool definitions, tool execution
    src/bookstack.rs     BookStack REST API client (reqwest)
    src/oauth.rs         OAuth 2.1 + refresh tokens, login form, token exchange
    src/semantic.rs      Semantic search (calls embedder /embed, queries db), webhook handler
    src/migrate.rs       SQLite → PostgreSQL migration tool
    src/llm.rs           LLM client (OpenRouter, Anthropic, OpenAI) for instance summary
    src/summary.rs       Instance summary generator (background, cached in DB)

  bsmcp-embedder/        Embedder binary (pluggable backends)
    src/main.rs          Job queue worker + HTTP /embed endpoint + provider selection
    src/embed.rs         Embedder trait + implementations (LocalEmbedder, OllamaEmbedder, OpenAIEmbedder)
    src/pipeline.rs      Embedding pipeline (fetch pages, chunk, embed, store)
```

**Two transports:**
1. **SSE (MCP 2024-11-05):** Client connects GET `/mcp/sse` with Bearer token -> validates -> creates session -> client sends JSON-RPC to `/mcp/messages/?sessionId=<id>` -> response via SSE event.
2. **Streamable HTTP (MCP 2025-03-26):** Client POSTs JSON-RPC to `/mcp/sse` with Bearer token -> validates -> returns JSON response directly. Used by claude.ai.

**Key patterns:**
- `mcp.rs` uses `block_in_place` + `block_on` to call async BookStack client from sync `handle_request`
- Tool definitions use helper fns: `tool()`, `paginated_schema()`, `id_schema()`, `name_desc_schema()`, `update_schema()`
- `bookstack.rs` has 4 HTTP methods (`get`, `post`, `put`, `delete`) that all follow the same pattern
- Sessions stored in `Arc<RwLock<HashMap<String, Session>>>` with 30s cleanup loop
- Database operations go through `dyn DbBackend` / `dyn SemanticDb` trait objects
- Server selects backend at startup via `BSMCP_DB_BACKEND` env var

**Semantic search flow:**
1. Server receives `semantic_search` tool call
2. Server POSTs query text to embedder's `/embed` endpoint → gets query embedding
3. Server calls `db.vector_search()` → SQLite does brute-force cosine, PostgreSQL uses pgvector HNSW
4. Server calls `db.get_markov_blanket()` for contextual relationships
5. Returns ranked results with content snippets and relationship context

**Embedding flow:**
1. `reembed` tool or webhook inserts a job into `embed_jobs` table
2. Embedder polls `embed_jobs` for pending jobs
3. Embedder fetches pages from BookStack API, chunks content, generates embeddings
4. Embedder stores chunks + embeddings in database, updates job progress

## Environment Variables

All prefixed `BSMCP_`. See `.env.example` for full list. Key ones:

**Server:**
- `BSMCP_BOOKSTACK_URL` (required)
- `BSMCP_ENCRYPTION_KEY` (required, 32+ chars)
- `BSMCP_DB_BACKEND` — `sqlite` (default) or `postgres`
- `BSMCP_DATABASE_URL` — PostgreSQL connection string (required if postgres)
- `BSMCP_DB_PATH` — SQLite path (default: `/data/bookstack-mcp.db`)
- `BSMCP_PUBLIC_DOMAIN` (for OAuth redirect URLs)
- `BSMCP_SEMANTIC_SEARCH` — `true` to enable semantic tools
- `BSMCP_EMBEDDER_URL` — embedder HTTP endpoint (default: `http://bsmcp-embedder:8081`)
- `BSMCP_WEBHOOK_SECRET` — constant-time verified webhook secret

**Embedder:**
- `BSMCP_EMBED_TOKEN_ID` / `BSMCP_EMBED_TOKEN_SECRET` — BookStack API token for crawling
- `BSMCP_EMBED_PROVIDER` — `local` (default ONNX), `ollama`, `openai`
- `BSMCP_EMBED_MODEL` — model name (default per provider)
- `BSMCP_EMBED_API_KEY` — API key (openai provider only)
- `BSMCP_EMBED_API_URL` — base URL for ollama/openai
- `BSMCP_EMBED_DIMS` — embedding dimensions (auto-detected for ollama)
- `BSMCP_EMBED_BATCH_SIZE`, `BSMCP_EMBED_DELAY_MS` — performance tuning

**Instance Summary (optional):**
- `BSMCP_LLM_PROVIDER` — `openrouter`, `anthropic`, `openai`, `ollama`
- `BSMCP_LLM_API_KEY` — API key for LLM (not needed for ollama)
- `BSMCP_LLM_MODEL` — model ID (defaults per provider)
- `BSMCP_LLM_API_URL` — base URL (useful for ollama on different host)
- `BSMCP_SUMMARY_INTERVAL` — hours between regenerations (0 = only on first startup)
- `BSMCP_SUMMARY_TOKEN_ID/SECRET` — BookStack token (falls back to BSMCP_EMBED_TOKEN_*)

## Implemented Tools (58)

- **search_content** - Full-text search with BookStack query operators
- **semantic_search** - Natural language vector search (when semantic enabled)
- **reembed** - Trigger re-embedding of all pages (when semantic enabled)
- **embed_status** - Check embedding job status (when semantic enabled)
- **Shelves** - list, get, create, update (assign books), delete (5)
- **Books** - list, get, create, update, delete (5)
- **Chapters** - list, get, create, update (move between books), delete (5)
- **Pages** - list, get, create, update (move between chapters/books), delete (5)
- **Attachments** - list, get, create, upload, update, delete (6)
- **Exports** - export_page, export_chapter, export_book (3) - markdown, plaintext, or html
- **Comments** - list, get, create, update, delete (5)
- **Recycle Bin** - list, restore, destroy (3)
- **Users** - list, get (2) - read-only
- **Audit Log** - list (1)
- **System** - get_system_info (1)
- **Image Gallery** - list, get, upload, update, delete (5)
- **Content Permissions** - get, update (2)
- **Roles** - list, get (2) - read-only

## Not Implemented

- **Imports** - ZIP file handling doesn't work well over MCP text protocol.
- **User/Role CRUD** - Creating/deleting users/roles is admin-level; read-only is sufficient.
- **PDF/ZIP export** - Binary formats can't be returned as MCP text content.


## Adding a New Tool

1. **bookstack.rs** - Add the API method(s) to `BookStackClient`
2. **mcp.rs** - Add match arm in `execute_tool()`, add tool def in `tool_definitions()`
3. Use existing helpers: `arg_str`, `arg_i64`, `arg_i64_required`, `arg_str_default`, `filter_update_fields`, `format_json`

For GET endpoints that need a raw text response (like export), add a `get_text()` method to `BookStackClient` that returns `String` instead of `Value`.

## OAuth / Claude Desktop Custom Connector

The server implements OAuth 2.1 (authorization code + PKCE) with a browser-based login form for BookStack API token authentication.

**MCP endpoint URL:** `https://your-host/mcp/sse` (must include the `/mcp/sse` path)

**OAuth endpoints:**
- `GET /.well-known/oauth-authorization-server` — RFC 8414 metadata (MCP 2025-03-26)
- `GET /.well-known/oauth-protected-resource` — RFC 9728 metadata (MCP 2025-06-18)
- `GET /authorize` — Serves login form for API token entry
- `POST /authorize` — Validates token against BookStack, issues auth code
- `POST /token` — Token exchange

**Two auth flows:**
1. **Form-based (primary):** Claude opens /authorize → user enters BookStack API token → server validates → stores credentials with auth code → redirects → code exchange issues access token.
2. **Legacy Bearer:** `Authorization: Bearer token_id:token_secret` on SSE/messages endpoints (Claude Code direct connection).

## Building

```bash
# Local build (both binaries)
cargo build --release

# Individual
cargo build --release -p bsmcp-server
cargo build --release -p bsmcp-embedder

# Multi-arch Docker
docker buildx build --builder multiarch --platform linux/amd64,linux/arm64 \
  -f docker/Dockerfile.server \
  -t ghcr.io/bees-roadhouse/bsmcp-server:VERSION --push .

docker buildx build --builder multiarch --platform linux/amd64,linux/arm64 \
  -f docker/Dockerfile.embedder \
  -t ghcr.io/bees-roadhouse/bsmcp-embedder:VERSION --push .
```

Images:
- `ghcr.io/bees-roadhouse/bsmcp-server` — MCP server (~35MB)
- `ghcr.io/bees-roadhouse/bsmcp-embedder` — Embedder with ONNX (~45MB)

## Docker Compose

Two deployment options:

- `docker/docker-compose.yml` — PostgreSQL (bsmcp-postgres + bsmcp-server + bsmcp-embedder)
- `docker/docker-compose.sqlite.yml` — SQLite (bsmcp-server + bsmcp-embedder sharing a file)

## Migration

**SQLite → PostgreSQL auto-migration:** When `BSMCP_DB_BACKEND=postgres` and a SQLite DB exists at `BSMCP_DB_PATH`, the server auto-migrates on startup and renames the file to `.db.migrated`.

**Manual migration:**
```bash
bsmcp-server migrate --from-sqlite /path/to/db --to-postgres postgres://user:pass@host/db
```

Migrates: access_tokens, pages, chunks (BLOB→pgvector), relationships, embed_jobs. Validates row counts.

## Branch Info

- `development` - default branch, active work lands here
- `release` - stable/production branch, merged from development when ready
- `enhancement/{name}` - branched from development for new functionality
- `problem/{name}` - branched from development for bug fixes

## Breaking Changes Log

### v0.3.0 (from v0.2.x)
- **Two images:** `ghcr.io/bees-roadhouse/bsmcp-server` + `ghcr.io/bees-roadhouse/bsmcp-embedder` (was single `bookstack-mcp`)
- **New env vars:** `BSMCP_DB_BACKEND`, `BSMCP_DATABASE_URL`, `BSMCP_EMBEDDER_URL`, `BSMCP_EMBED_TOKEN_*`, performance tuning vars
- **Docker service renames:** `postgres` → `bsmcp-postgres`, `bookstack-mcp` → `bsmcp-server`, `pgdata` → `bsmcp-pgdata`
- **Embedder is separate:** No more in-process ONNX model; embedder runs as its own container/binary

### v0.1.3 (from v0.1.2)
- `BSMCP_ENCRYPTION_KEY` now **required** — server panics without it
- `BSMCP_PUBLIC_URL` removed, replaced by `BSMCP_PUBLIC_DOMAIN` (domain only, not full URL)
- Docker volume renamed `mcp-data` → `bsmcp-data` — data migration needed
- OAuth now enforces PKCE (S256)

## Git Workflow

**Branching Model:**
* `development` — default branch (HEAD), active work lands here
* `release` — stable/production branch, merged from development when ready
* `enhancement/{name}` — branched from development for new functionality
* `problem/{name}` — branched from development for bug fixes and issue resolution
* No `main` or `master` branches

**GitHub Issues & Labels:**
* New functionality uses the **enhancement** label, not "feature"
* Configure repository labels accordingly
