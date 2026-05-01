# BookStack MCP Server

Rust MCP server that bridges Claude to a BookStack instance via SSE and Streamable HTTP transports. Organized as a Cargo workspace with pluggable database backends and a separate embedder service.

> Build, branching, CI/CD, versioning, and contributor workflow live in [DEVELOPMENT.md](DEVELOPMENT.md). This file is the architectural map and project-shape context for AI agents working in the codebase.

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
    src/session.rs       Per-(token_hash, session_id) state for meta.briefing
                         auto-injection — first-call vs sticky decisions
    src/briefing/        Briefing flow (POST /briefing/v1/read + `briefing` tool)
      mod.rs             Entry point — loads settings, runs builder, wraps envelope
      briefing.rs        Builder: time + identity + system_prompt_additions +
                         setup_status + KB semantic matches against the prompt
      envelope.rs        {ok, data, meta, error} response shape, time block helper
      frontmatter.rs     Provenance YAML-frontmatter helpers (legacy, low-traffic)
    src/oauth.rs         OAuth 2.1 + refresh tokens, login form, token exchange
    src/semantic.rs      Semantic search (calls embedder /embed, queries db), webhook handler
    src/settings_ui.rs   Browser-based /settings form (token-gated via cookie)
    src/staging.rs       File staging for upload_image / upload_attachment
    src/migrate.rs       SQLite → PostgreSQL migration tool
    src/llm.rs           LLM client (OpenRouter, Anthropic, OpenAI) for instance summary
    src/summary.rs       Instance summary generator (background, cached in DB)

  bsmcp-embedder/        Embedder binary (pluggable backends)
    src/main.rs          Job queue worker + HTTP /embed endpoint + provider selection
    src/embed.rs         Embedder trait + implementations (LocalEmbedder, OllamaEmbedder, OpenAIEmbedder)
    src/pipeline.rs      Embedding pipeline (fetch pages, chunk, embed, store)

  bsmcp-worker/          Reconciliation worker binary (v1.1.0+)
    src/main.rs          Env wiring, db init, BookStackClient, IndexWorker spawn
    src/lib.rs           IndexWorker — owns the index_jobs queue. Initial full
                         walk on cold start, polls for webhook + cron jobs,
                         runs the periodic delta walk. Same database as the
                         server (server's webhook handler enqueues; worker
                         consumes).
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

## Briefing flow (`/briefing`)

Server-side per-session reconstitution. The personal-memory layer (journals, collages, identities, whoami, user) moved to memberberry.ai in v0.8.0 — what remains here is the **briefing**: a single response shape that gives the AI everything it needs about the current session.

**HTTP:** `POST /briefing/v1/read` — JSON body in, JSON envelope out (`{ok, data, meta, error}`). Auth via the same Bearer token as `/mcp/sse`.

**MCP:** one tool, `briefing`. Optional `user_prompt` (drives semantic prioritization), `client_timezone`, `session_id`. No `action` dispatch.

**Response shape (`data`):** time block, org/user identity context, `system_prompt_additions` (guide page, org_identity, org_required_instructions, org_ai_usage_policy, user `system_prompt_page_ids`, owned-domains synthetic block), `setup_status` / `setup_nudge` when settings are incomplete, `kb_semantic_matches` against the `user_prompt`, and a thin config echo.

**Auto-injection on every MCP tool response:** the briefing payload is also added as `meta.briefing` on every other tool call's response. Full content on the first call per session, sticky-only (time + setup_summary) thereafter. State is keyed by `(token_hash, session_id)` and tracked in `crates/bsmcp-server/src/session.rs`. When `session_id` is absent (Streamable HTTP is stateless), the server falls back to a per-hour bucket per token. Calling the `briefing` tool explicitly resets the session so the next response carries full content again — useful after compaction.

**Settings backing the briefing:**
- Per-user `user_settings` (JSON blob): label, role, user_id, timezone, owned `domains`, `system_prompt_page_ids` (always-on context — short, durable docs like style guides), `semantic_against_full_kb` toggle.
- Global `global_settings` (single row): typed setup slots `guide_page_id`, `org_identity_page_id`, `policies_scope`, `sops_scope`, `best_practices_scope`; always-on lists `org_required_instructions_page_ids`, `org_ai_usage_policy_page_ids`, `org_domains`; org-wide booleans `friendly_structure`, `full_content_in_briefing`, `strict_setup`.

Null settings just omit the affected section from the briefing; with `strict_setup=true` they instead surface `setup_required` errors on tool calls until configured.

## Settings UI (`/settings`)

Browser-based config page. Token-gated via the `/authorize` form — when `?return_to=/settings` is set, the server validates the BookStack API token and issues a settings-session cookie (HttpOnly, 8h TTL, in-memory store) instead of running the full OAuth code dance.

In v0.8.0 the UI is a minimal text-input form covering the surviving fields. Most of the per-user pointers it used to manage (journal/collage/identity book IDs, recent-counts) are gone — those books and the resources that read them no longer exist. Typeahead pickers backed by `precision_search` are planned as a follow-up.

**Form fields:**
- *Per-user (any authenticated user):* `label`, `role`, `user_id`, `bookstack_user_id`, owned `domains`, `system_prompt_page_ids`, `timezone`, `semantic_against_full_kb`.
- *Global (admins only — server checks `/api/users` access before persisting):* `guide_page_id`, `org_identity_page_id`, `policies_scope`, `sops_scope`, `best_practices_scope`, `org_required_instructions_page_ids`, `org_ai_usage_policy_page_ids`, `org_domains`, `friendly_structure`, `full_content_in_briefing`, `strict_setup`, plus the legacy `hive_shelf_id` / `user_journals_shelf_id` pointers (kept for the index worker / directory listings).

Non-admin users can submit the form; admin-only fields are silently dropped server-side. There is no MCP write path for global settings — they must be configured via `/settings` by an admin.

**Probe (`/settings/probe`):** disabled in v0.8.0 (returns 410). Auto-discovery for the new typed slots is a follow-up design.

## Auth-gated `/status`

The semantic-search status page accepts either a Bearer token (programmatic) or a settings-session cookie (browser). Unauthenticated requests get a 401 with a link to `/settings`.

## Global settings (`global_settings` table)

Single-row table holding instance-wide pointers used by the briefing builder, semantic search, and the index worker. Key fields:

- **Typed setup slots:** `guide_page_id`, `org_identity_page_id`, `policies_scope`, `sops_scope`, `best_practices_scope` — drive `system_prompt_additions` and bias semantic results.
- **Always-on lists:** `org_required_instructions_page_ids`, `org_ai_usage_policy_page_ids`, `org_domains` — included verbatim in every briefing.
- **Org-wide booleans:** `friendly_structure`, `full_content_in_briefing`, `strict_setup`.
- **Index pointers:** `hive_shelf_id`, `user_journals_shelf_id` — used by the reconciliation worker / directory listings.
- `set_by_token_hash` — the first user who configured them (informational; does not gate writes).

Writes are admin-only (BookStack admin probed via `/api/users` access in the settings handler).

## Implemented Tools (62 + 1 briefing)

- **search_content** - Full-text search with BookStack query operators
- **semantic_search** - Natural language vector search (when semantic enabled)
- **reembed** - Trigger re-embedding of all pages (when semantic enabled)
- **embedding_status** - Check semantic index status (when semantic enabled)
- **briefing** - Per-session reconstitution shell (also auto-injected as `meta.briefing` on every tool response)
- **Shelves** - list, get, create, update (assign books), delete (5)
- **Books** - list, get, create, update, delete (5)
- **Chapters** - list, get, create, update (move between books), delete (5)
- **Pages** - list, get, create, update (move between chapters/books), delete (5)
- **Page edits (partial)** - edit_page, append_to_page, replace_section, insert_after (4)
- **Move** - move_page, move_chapter, move_book_to_shelf (3) - dedicated move operations
- **Attachments** - list, get, create, upload, update, delete (6)
- **Staging** - prepare_upload (1) - create a staging slot for local-file uploads
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

## Breaking Changes Log

### v0.8.0 (from v0.7.x)
- **All `remember_*` MCP tools removed.** The personal-memory layer (journals, collages, identities, whoami, user) moved to memberberry.ai. The 12 `remember_briefing` / `remember_journal` / `remember_collage` / `remember_shared_collage` / `remember_user_journal` / `remember_whoami` / `remember_user` / `remember_identity` / `remember_directory` / `remember_config` / `remember_audit` / `remember_search` tools no longer ship with the server.
- **HTTP namespace replaced.** `POST /remember/v1/{resource}/{action}` is gone; the surviving briefing surface is `POST /briefing/v1/read` only.
- **Single `briefing` MCP tool replaces the 12 remember tools.** Same response shape as the old `remember_briefing action=read`, no `action` arg.
- **`meta.briefing` auto-injected on every MCP tool response.** Full content on the first call per session, sticky-only (time + setup_summary) thereafter. Driven by per-`(token_hash, session_id)` state in `crates/bsmcp-server/src/session.rs`. Clients without a `session_id` collapse into a stable `{token_hash}:no-session` slot — first call gets full briefing, sticky thereafter. Calling the `briefing` tool or `session_event action=compacted` resets the session for the next response.
- **`UserSettings` dropped fields:** `ai_hive_journal_book_id`, `ai_collage_book_id`, `ai_shared_collage_book_id`, `ai_identity_page_id`, `user_journal_book_id`, `user_identity_page_id`, plus all `recent_*_count` fields. No DB migration required — `user_settings` is a JSON blob, old keys are silently ignored on read and dropped on next save.
- **`GlobalSettings` gained typed setup slots:** `guide_page_id`, `org_identity_page_id`, `policies_scope`, `sops_scope`, `best_practices_scope`, plus org-wide booleans `friendly_structure`, `full_content_in_briefing`, `strict_setup`. Idempotent `ALTER TABLE ADD COLUMN` migrations on first startup; new installs include them in `CREATE TABLE`.
- **`default_ai_identity_*` global columns dropped.** Removed from `CREATE TABLE` / `ALTER TABLE` paths and actively dropped from existing installs via idempotent `ALTER TABLE DROP COLUMN [IF EXISTS]` migrations on startup (Postgres `IF EXISTS`; SQLite swallows the duplicate-drop error via `.ok()`, requires SQLite ≥ 3.35).
- **`remember_audit` table dropped.** `DROP TABLE IF EXISTS remember_audit` on startup for both backends. Any v0.7.x audit-log data on disk is destroyed during this migration; the rows had no consumers post-`3d9370f`.
- **Settings UI gutted (~1,300 lines deleted).** Most per-user pointer fields are gone since the books they pointed to no longer exist; what remains is a minimal text-input form for the surviving fields.

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
